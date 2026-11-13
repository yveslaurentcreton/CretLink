//! One activity item for both surfaces (ST-M009-011, REQ-F-017, REQ-F-026).
//!
//! The technician console and the customer window show the **same** item for
//! every action on the customer computer: who started it ([`ActivitySource`]),
//! the AI's stated purpose (AI actions only), a plain description, what kind of
//! action it is ([`ActivityKind`]), the real operation, the path and byte count
//! when there are any, the honest state ([`ActivityState`]), the start time and
//! the duration. This module holds that one type, the bounded per-session
//! [`ActivityFeed`] both roles keep, and the customer-side fold
//! ([`CustomerActivity`]) that turns the agent's [`SessionEvent`]s into items.
//! The technician-side fold, which reads the console's audit records, lives in
//! `cretlink-console` beside those records.
//!
//! Every item serializes exactly to `ActivityItem` / `ActivityEntry` in the
//! application's frontend contract (`apps/app/src/shared/contract.ts`):
//! camelCase fields, `state` and `entry` as tagged unions, `startedAt` in
//! RFC 3339 UTC. Every string is built here, never by the frontend, which writes
//! the untrusted parts (operations, paths) as text, never as HTML (REQ-S-016,
//! REQ-S-018).
//!
//! # No silent gaps
//!
//! A feed never pretends to be complete when it is not. Events shed upstream
//! under pressure arrive as a count and become an [`ActivityEntry::Gap`] at the
//! place where they were dropped; items evicted because the feed reached its
//! bound are counted into a leading gap.

use std::collections::{HashMap, VecDeque};

use cretlink_protocol::{ErrorCode, JobId, JobOutcome, Operation, Origin, RemotePath};
use cretlink_shared::format_rfc3339_millis;
use serde::{Deserialize, Serialize};

use crate::action::{ActionOutcome, describe_exec};
use crate::{MAX_ACTION_DESCRIPTION_CHARS, SessionEvent};

/// The most entries one [`ActivityFeed`] keeps (REQ-N-006). Older entries are
/// evicted first and counted into a leading [`ActivityEntry::Gap`].
pub const MAX_FEED_ENTRIES: usize = 1000;

/// Who started an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    /// An AI client through the MCP bridge.
    Ai,
    /// The technician's interactive terminal (REQ-S-023).
    Terminal,
    /// Any other technician action (a manual command, a transfer, a stop).
    Technician,
}

impl From<Origin> for ActivitySource {
    fn from(origin: Origin) -> Self {
        match origin {
            Origin::Mcp => Self::Ai,
            Origin::Terminal => Self::Terminal,
            Origin::Console => Self::Technician,
        }
    }
}

/// What an action does to the customer computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// A command or an interactive terminal.
    Command,
    /// A file left the customer computer (REQ-S-016).
    FileSent,
    /// A file arrived on the customer computer.
    FileReceived,
    /// Any other file or folder operation.
    FileOperation,
    /// The screen was observed (a screenshot).
    ScreenObserved,
    /// The mouse or keyboard was driven.
    ScreenControlled,
    /// The clipboard text was read.
    ClipboardRead,
    /// The clipboard text was replaced.
    ClipboardWritten,
    /// System information was read.
    SystemInformation,
    /// A running job was fed input, resized, listed, or stopped.
    JobControl,
}

impl ActivityKind {
    /// The kind of one wire operation, derived by the agent from the request it
    /// handles, so it is known against every console version.
    #[must_use]
    pub fn of(op: &Operation) -> Self {
        match op {
            Operation::SystemInfo {} => Self::SystemInformation,
            Operation::Exec { .. } | Operation::TermOpen { .. } => Self::Command,
            Operation::WriteStdin { .. }
            | Operation::ListJobs {}
            | Operation::TermResize { .. }
            | Operation::TermClose { .. } => Self::JobControl,
            Operation::FsRead { .. } => Self::FileSent,
            Operation::FsCommit { .. } => Self::FileReceived,
            Operation::FsStat { .. }
            | Operation::FsList { .. }
            | Operation::CreateDir { .. }
            | Operation::FsWrite { .. }
            | Operation::FsAbort { .. }
            | Operation::FsDigest { .. }
            | Operation::FsMove { .. }
            | Operation::FsDelete { .. }
            | Operation::RemoveDir { .. }
            | Operation::ListStaged {}
            | Operation::StagingCleanup { .. }
            | Operation::FsDrives {}
            | Operation::RemoveTree { .. } => Self::FileOperation,
            Operation::DesktopCapture { .. } => Self::ScreenObserved,
            Operation::DesktopPointer { .. }
            | Operation::DesktopScroll { .. }
            | Operation::DesktopKey { .. }
            | Operation::DesktopType { .. }
            | Operation::DesktopHotkey { .. }
            | Operation::DesktopWait { .. } => Self::ScreenControlled,
            Operation::ClipboardGet {} => Self::ClipboardRead,
            Operation::ClipboardSet { .. } => Self::ClipboardWritten,
        }
    }
}

/// The honest state of an action. `Failed` covers every outcome that is not a
/// success, including an unknown one, which is never shown as done
/// (REQ-N-008).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ActivityState {
    /// Still running.
    Running,
    /// Finished; a command carries its exit code.
    Done {
        /// The process exit code, when there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// Failed, refused, or ended with an unknown outcome.
    Failed {
        /// The process exit code, when there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// A plain, secret-free reason, when one is known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Cancelled before it finished.
    Cancelled,
    /// Stopped at its deadline.
    TimedOut,
}

impl ActivityState {
    /// A failure with only a reason.
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            exit_code: None,
            message: Some(message.into()),
        }
    }

    /// The state of a finished job, from its honest outcome and exit code.
    #[must_use]
    pub fn of_job(outcome: JobOutcome, exit_code: Option<i32>) -> Self {
        match outcome {
            JobOutcome::Completed => Self::Done { exit_code },
            JobOutcome::Failed => Self::Failed {
                exit_code,
                message: None,
            },
            JobOutcome::Cancelled => Self::Cancelled,
            JobOutcome::TimedOut => Self::TimedOut,
            JobOutcome::Unknown => Self::Failed {
                exit_code,
                message: Some(UNKNOWN_RESULT.to_owned()),
            },
        }
    }

    /// The state of one handled request, from how the agent answered it.
    #[must_use]
    pub fn of_outcome(outcome: ActionOutcome) -> Self {
        match outcome {
            ActionOutcome::InProgress => Self::Running,
            ActionOutcome::Succeeded => Self::Done { exit_code: None },
            ActionOutcome::Failed { code } => Self::failed(error_text(code)),
            ActionOutcome::Cancelled => Self::Cancelled,
            ActionOutcome::Unknown => Self::failed(UNKNOWN_RESULT),
        }
    }

    /// Whether the action is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// The reason shown when no answer arrived, so the outcome is not known.
pub const UNKNOWN_RESULT: &str = "the result is not known";

/// The reason shown for an action still running when the session ended.
pub const SESSION_ENDED: &str = "the session ended before this finished";

/// One activity item, identical on both surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityItem {
    /// Stable for the life of the item, so a running item is updated in place
    /// when it finishes.
    pub id: String,
    /// Who started the action.
    pub source: ActivitySource,
    /// Technician side only: the AI client's label ("Claude Code", "Codex
    /// CLI", "Gemini CLI", or "AI assistant"). Never sent to the customer
    /// computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_label: Option<String>,
    /// The AI's stated purpose: an unverified claim, shown beside `operation`,
    /// never instead of it (REQ-F-026). AI actions only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// A plain description, always present ([`describe`]).
    pub description: String,
    /// What the action does.
    pub kind: ActivityKind,
    /// The real operation in plain words, bounded and free of typed, clipboard,
    /// stdin, or file content.
    pub operation: String,
    /// The remote path, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The byte count, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// The honest state.
    pub state: ActivityState,
    /// When the action started, RFC 3339 UTC with milliseconds.
    pub started_at: String,
    /// How long it took, once it finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// One entry of a feed: an item, or a marker that `count` items were not shown
/// at this place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "almost every entry is an item; a gap is rare, so boxing items would only add an allocation per item"
)]
pub enum ActivityEntry {
    /// An activity item.
    Item {
        /// The item.
        item: ActivityItem,
    },
    /// `count` items were not shown here.
    Gap {
        /// How many items were not shown.
        count: u64,
        /// When they were dropped, RFC 3339 UTC.
        at: String,
    },
}

/// A plain description of an action without a purpose.
#[must_use]
pub fn describe(kind: ActivityKind, source: ActivitySource) -> &'static str {
    match kind {
        ActivityKind::Command => match source {
            ActivitySource::Terminal => "Used the terminal",
            ActivitySource::Ai | ActivitySource::Technician => "Ran a command",
        },
        ActivityKind::FileSent => "Copied a file from the customer computer",
        ActivityKind::FileReceived => "Copied a file to the customer computer",
        ActivityKind::FileOperation => "Worked with files and folders",
        ActivityKind::ScreenObserved => "Took a picture of the screen",
        ActivityKind::ScreenControlled => "Used the mouse or keyboard",
        ActivityKind::ClipboardRead => "Read the clipboard",
        ActivityKind::ClipboardWritten => "Changed the clipboard",
        ActivityKind::SystemInformation => "Read system information",
        ActivityKind::JobControl => "Managed a running command",
    }
}

/// The remote path one operation acts on (the source of a move), in the same
/// words [`crate::describe_operation`] uses, bounded.
#[must_use]
pub fn path_of(op: &Operation) -> Option<String> {
    let path = match op {
        Operation::FsStat { path }
        | Operation::FsList { path, .. }
        | Operation::CreateDir { path }
        | Operation::FsRead { path, .. }
        | Operation::FsDigest { path, .. }
        | Operation::FsDelete { path }
        | Operation::RemoveDir { path }
        | Operation::RemoveTree { path, .. } => path,
        Operation::FsWrite { dest, .. }
        | Operation::FsCommit { dest, .. }
        | Operation::FsAbort { dest } => dest,
        Operation::FsMove { from, .. } => from,
        _ => return None,
    };
    Some(bounded(remote_path_text(path)))
}

/// The byte count one request carries by itself (the committed length of a
/// received file, the bytes written by a chunk). The bytes of a file leaving
/// the computer come from the agent's answers instead.
#[must_use]
pub fn bytes_of(op: &Operation) -> Option<u64> {
    match op {
        Operation::FsCommit { len, .. } => Some(*len),
        Operation::FsWrite { data, .. } => Some(data.len() as u64),
        _ => None,
    }
}

fn remote_path_text(path: &RemotePath) -> String {
    match path {
        RemotePath::Absolute { path } => path.clone(),
        RemotePath::Staging { name } => format!("{name} (CretLink staging folder)"),
    }
}

/// Bounds a text field like an operation description, marking a cut.
pub(crate) fn bounded(text: String) -> String {
    if text.chars().count() <= MAX_ACTION_DESCRIPTION_CHARS {
        return text;
    }
    let mut out: String = text.chars().take(MAX_ACTION_DESCRIPTION_CHARS).collect();
    out.push_str(" [shortened]");
    out
}

/// A plain reason for a refused or failed request.
#[must_use]
pub fn error_text(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::IncompatibleVersion => "the two computers run incompatible versions",
        ErrorCode::MalformedMessage | ErrorCode::OversizedMessage => "the request was malformed",
        ErrorCode::LimitExceeded => "the request exceeded a limit",
        ErrorCode::UnknownSession => "the request was not for this session",
        ErrorCode::UnknownJob => "no such running command",
        ErrorCode::SessionRevoked => "the session was stopped",
        ErrorCode::Unsupported => "not supported on this computer",
        ErrorCode::PathNotFound => "the path does not exist",
        ErrorCode::AccessDenied => "access was denied",
        ErrorCode::FileInUse => "the file is in use",
        ErrorCode::InvalidPath => "the path was refused",
        ErrorCode::AlreadyExists => "the destination already exists",
        ErrorCode::WrongPathKind => "the path is not the expected kind",
        ErrorCode::DirNotEmpty => "the folder is not empty",
        ErrorCode::WriteConflict => "the file changed during the transfer",
        ErrorCode::IntegrityMismatch => "the file did not match its checksum",
        ErrorCode::StorageExhausted => "the disk is full",
        ErrorCode::OutsideStagingRoot => "the path is outside the CretLink staging folder",
        ErrorCode::Internal => "an internal error occurred",
    }
}

/// The bounded feed of one session, newest last (REQ-N-006).
///
/// An item whose id is already in the feed replaces the earlier one in place,
/// so a running item becomes done where it was first shown. When the feed is
/// full the oldest entry is evicted and counted into a leading gap, so the
/// feed never looks complete when it is not.
#[derive(Debug, Clone)]
pub struct ActivityFeed {
    entries: VecDeque<ActivityEntry>,
    /// Entries evicted at the front, and when the first of them was dropped.
    evicted: Option<(u64, String)>,
    capacity: usize,
}

impl Default for ActivityFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityFeed {
    /// An empty feed bounded by [`MAX_FEED_ENTRIES`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(MAX_FEED_ENTRIES)
    }

    /// An empty feed bounded by `capacity` entries (at least one).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            evicted: None,
            capacity: capacity.max(1),
        }
    }

    /// Inserts `item`, or replaces the item with the same id in place, and
    /// returns the entry to emit.
    pub fn upsert(&mut self, item: ActivityItem) -> ActivityEntry {
        let slot = self.entries.iter_mut().rev().find(
            |existing| matches!(existing, ActivityEntry::Item { item: old } if old.id == item.id),
        );
        let entry = ActivityEntry::Item { item };
        match slot {
            Some(slot) => *slot = entry.clone(),
            None => self.push(entry.clone()),
        }
        entry
    }

    /// Records that `count` items were not shown at this place, and returns
    /// the gap entry to emit.
    pub fn gap(&mut self, count: u64, at: String) -> ActivityEntry {
        let entry = ActivityEntry::Gap { count, at };
        self.push(entry.clone());
        entry
    }

    /// The item with `id`, if the feed still holds it.
    #[must_use]
    pub fn item(&self, id: &str) -> Option<&ActivityItem> {
        self.entries.iter().rev().find_map(|entry| match entry {
            ActivityEntry::Item { item } if item.id == id => Some(item),
            _ => None,
        })
    }

    /// Every entry, oldest first, with a leading gap for evicted entries.
    #[must_use]
    pub fn entries(&self) -> Vec<ActivityEntry> {
        let mut out = Vec::with_capacity(self.entries.len() + 1);
        if let Some((count, at)) = &self.evicted {
            out.push(ActivityEntry::Gap {
                count: *count,
                at: at.clone(),
            });
        }
        out.extend(self.entries.iter().cloned());
        out
    }

    /// How many entries the feed holds (without the eviction gap).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the feed holds no entries and evicted none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.evicted.is_none()
    }

    fn push(&mut self, entry: ActivityEntry) {
        if self.entries.len() >= self.capacity
            && let Some(oldest) = self.entries.pop_front()
        {
            let (dropped, at) = match oldest {
                ActivityEntry::Item { item } => (1, item.started_at),
                ActivityEntry::Gap { count, at } => (count, at),
            };
            match &mut self.evicted {
                Some((count, _)) => *count += dropped,
                None => self.evicted = Some((dropped, at)),
            }
        }
        self.entries.push_back(entry);
    }
}

/// The customer-side fold: turns the agent's [`SessionEvent`]s into activity
/// items (ST-M009-011).
///
/// A started command becomes a running item that its `ExecFinished` updates in
/// place; a handled request becomes an item keyed by its id, so a file leaving
/// the computer is shown when sending starts and updated when it ends; shed
/// events become a gap at the place they were dropped. When the session ends,
/// an item still running is shown as failed because the session ended, never
/// as done (REQ-N-008). The client label never exists on this side: the AI is
/// shown as the AI assistant.
#[derive(Debug, Clone, Default)]
pub struct CustomerActivity {
    feed: ActivityFeed,
    /// Running jobs: the item id each job's `ExecFinished` updates.
    jobs: HashMap<JobId, String>,
    /// When each running item started, in Unix milliseconds.
    started: HashMap<String, i64>,
}

impl CustomerActivity {
    /// An empty fold.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one event observed at `now_unix_millis` and returns the entries
    /// that changed, in order (often none: lifecycle events carry no item).
    pub fn apply(&mut self, event: &SessionEvent, now_unix_millis: i64) -> Vec<ActivityEntry> {
        match event {
            SessionEvent::CommandStarted {
                job,
                shell,
                command,
                origin,
                purpose,
            } => {
                let id = format!("job-{job}");
                let source = ActivitySource::from(*origin);
                self.jobs.insert(*job, id.clone());
                self.started.insert(id.clone(), now_unix_millis);
                vec![self.feed.upsert(ActivityItem {
                    id,
                    source,
                    client_label: None,
                    purpose: purpose.as_ref().map(|p| p.as_str().to_owned()),
                    description: describe(ActivityKind::Command, source).to_owned(),
                    kind: ActivityKind::Command,
                    operation: describe_exec(*shell, command),
                    path: None,
                    bytes: None,
                    state: ActivityState::Running,
                    started_at: format_rfc3339_millis(now_unix_millis),
                    duration_ms: None,
                })]
            }
            SessionEvent::ExecFinished {
                job,
                exit_code,
                outcome,
            } => {
                // A job this fold never saw start (an interactive terminal, or
                // a start that was shed and already counted) has no item.
                let Some(id) = self.jobs.remove(job) else {
                    return Vec::new();
                };
                self.finish(
                    &id,
                    ActivityState::of_job(*outcome, *exit_code),
                    now_unix_millis,
                )
                .into_iter()
                .collect()
            }
            SessionEvent::RequestHandled {
                id,
                origin,
                purpose,
                kind,
                operation,
                path,
                bytes,
                outcome,
                duration_ms,
            } => {
                let id = format!("req-{id}");
                let source = ActivitySource::from(*origin);
                let state = ActivityState::of_outcome(*outcome);
                let started = self.started.get(&id).copied().unwrap_or_else(|| {
                    now_unix_millis.saturating_sub(
                        duration_ms.map_or(0, |ms| i64::try_from(ms).unwrap_or(i64::MAX)),
                    )
                });
                if state.is_running() {
                    self.started.insert(id.clone(), started);
                } else {
                    self.started.remove(&id);
                }
                vec![self.feed.upsert(ActivityItem {
                    id,
                    source,
                    client_label: None,
                    purpose: purpose.as_ref().map(|p| p.as_str().to_owned()),
                    description: describe(*kind, source).to_owned(),
                    kind: *kind,
                    operation: operation.clone(),
                    path: path.clone(),
                    bytes: *bytes,
                    state,
                    started_at: format_rfc3339_millis(started),
                    duration_ms: *duration_ms,
                })]
            }
            SessionEvent::ActivityShed { count } => {
                vec![
                    self.feed
                        .gap(*count, format_rfc3339_millis(now_unix_millis)),
                ]
            }
            SessionEvent::Revoked { .. } | SessionEvent::Failed { .. } => {
                self.session_ended(now_unix_millis)
            }
            _ => Vec::new(),
        }
    }

    /// Marks every item still running as failed because the session ended,
    /// returning the changed entries.
    pub fn session_ended(&mut self, now_unix_millis: i64) -> Vec<ActivityEntry> {
        self.jobs.clear();
        let mut running: Vec<String> = self.started.keys().cloned().collect();
        running.sort();
        running
            .into_iter()
            .filter_map(|id| {
                self.finish(&id, ActivityState::failed(SESSION_ENDED), now_unix_millis)
            })
            .collect()
    }

    /// The whole feed, oldest first.
    #[must_use]
    pub fn entries(&self) -> Vec<ActivityEntry> {
        self.feed.entries()
    }

    /// Gives `id` its final state and duration, if the feed still holds it.
    fn finish(
        &mut self,
        id: &str,
        state: ActivityState,
        now_unix_millis: i64,
    ) -> Option<ActivityEntry> {
        let started = self.started.remove(id);
        let mut item = self.feed.item(id)?.clone();
        item.state = state;
        item.duration_ms =
            started.map(|at| u64::try_from(now_unix_millis.saturating_sub(at)).unwrap_or(0));
        Some(self.feed.upsert(item))
    }
}

#[cfg(test)]
mod tests;
