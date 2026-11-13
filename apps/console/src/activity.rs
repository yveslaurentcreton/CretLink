//! The technician-side activity fold (ST-M009-011, REQ-F-017, REQ-S-023).
//!
//! The console builds the same [`ActivityItem`] the customer window builds, but
//! from its own capture-time audit records ([`AuditEntry`]) rather than from
//! the agent's events: every action the console routes is attributed at the
//! producing site, so the technician's feed covers every audit actor that acts
//! on the customer computer —
//!
//! - [`Actor::Mcp`]: every AI tool call, including read-only ones, with the AI
//!   client's label ([`client_label`]) and its stated purpose;
//! - [`Actor::Terminal`]: the technician terminal opening and closing;
//! - [`Actor::Console`]: manual console commands and cancellations, and the
//!   technician's own file actions — the drive list, the removal of a folder
//!   with its content, a file sent into a folder (ST-M010-001).
//!
//! A call's `_start` record opens a running item and its completion record
//! (sharing the `call_id`, the `job_id` for a manual command, or the
//! `action_id` for a file action of the technician) updates the same item in
//! place with the honest state and the duration. Session
//! lifecycle records (pairing, consent, cleanup) and purely local registry
//! records (a terminal consumer attaching) are not actions on the customer
//! computer and produce no item.

use std::collections::HashMap;

use cretlink_session::activity::{SESSION_ENDED, UNKNOWN_RESULT, describe};
use cretlink_session::{ActivityEntry, ActivityFeed, ActivityItem, ActivityKind, ActivitySource};
use cretlink_session::{ActivityState, MAX_ACTION_DESCRIPTION_CHARS};
use cretlink_shared::{Actor, AuditArgs, AuditRecord, AuditValue, Outcome, format_rfc3339_millis};

use crate::ipc_server::AuditEntry;

/// The parts of an audit record the fold reads: a live [`AuditEntry`] or a
/// written [`AuditRecord`] read back from an audit file (ST-M009-012).
struct RecordView<'a> {
    actor: Actor,
    action: &'a str,
    args: &'a AuditArgs,
    outcome: Outcome,
    duration_ms: Option<u64>,
}

impl<'a> From<&'a AuditEntry> for RecordView<'a> {
    fn from(entry: &'a AuditEntry) -> Self {
        Self {
            actor: entry.actor,
            action: entry.action,
            args: &entry.args,
            outcome: entry.outcome,
            duration_ms: entry.duration_ms,
        }
    }
}

impl<'a> From<&'a AuditRecord> for RecordView<'a> {
    fn from(record: &'a AuditRecord) -> Self {
        Self {
            actor: record.actor,
            action: &record.action,
            args: &record.args,
            outcome: record.outcome,
            duration_ms: record.duration_ms,
        }
    }
}

/// The label shown for an AI client the console does not recognize, or one
/// that reported no name.
pub const UNKNOWN_CLIENT_LABEL: &str = "AI assistant";

/// Maps an MCP client's self-reported `clientInfo.name` to the label the
/// technician sees (ST-M009-011): "Claude Code", "Codex CLI", "Gemini CLI", or
/// "AI assistant" when unknown. The name is the client's own claim, so only
/// the known clients' published names are recognized.
#[must_use]
pub fn client_label(name: Option<&str>) -> &'static str {
    let Some(name) = name else {
        return UNKNOWN_CLIENT_LABEL;
    };
    let name = name.trim().to_ascii_lowercase();
    if name == "claude-code" || name.starts_with("claude-code ") || name == "claude code" {
        "Claude Code"
    } else if name.starts_with("codex") {
        "Codex CLI"
    } else if name.starts_with("gemini-cli") || name == "gemini" || name == "gemini cli" {
        "Gemini CLI"
    } else {
        UNKNOWN_CLIENT_LABEL
    }
}

/// What one audited tool or console action is, for the feed.
struct Action {
    kind: ActivityKind,
    source: ActivitySource,
    operation: String,
}

/// The technician-side fold over the console's audit records.
#[derive(Debug, Clone, Default)]
pub struct TechnicianActivity {
    feed: ActivityFeed,
    /// When each running item started, in Unix milliseconds.
    started: HashMap<String, i64>,
    /// The last id minted for a record that carries no correlation id.
    last_local: u64,
}

impl TechnicianActivity {
    /// An empty fold.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one audit record observed at `now_unix_millis`, returning the
    /// entry that changed, if the record is an action on the customer computer.
    pub fn apply(&mut self, entry: &AuditEntry, now_unix_millis: i64) -> Option<ActivityEntry> {
        self.fold(&RecordView::from(entry), now_unix_millis)
    }

    /// Folds one record read back from an audit file, written at
    /// `at_unix_millis` (its `ts`), exactly like a live one (ST-M009-012).
    pub fn apply_record(
        &mut self,
        record: &AuditRecord,
        at_unix_millis: i64,
    ) -> Option<ActivityEntry> {
        self.fold(&RecordView::from(record), at_unix_millis)
    }

    fn fold(&mut self, entry: &RecordView<'_>, now_unix_millis: i64) -> Option<ActivityEntry> {
        let (base, is_start) = match entry.action.strip_suffix("_start") {
            Some(base) => (base, true),
            None => (entry.action, false),
        };
        let action = action_of(entry, base)?;
        // A start record that already failed (a manual command the runtime
        // refused to start) is a one-shot completion: nothing is running.
        let is_start = is_start && entry.outcome == Outcome::Ok;
        let id = self.id_for(entry, base, is_start)?;
        let purpose = (action.source == ActivitySource::Ai)
            .then(|| arg_text(entry, "purpose"))
            .flatten();
        let client = (action.source == ActivitySource::Ai)
            .then(|| client_label(arg_text(entry, "client").as_deref()).to_owned());
        let path = arg_text(entry, "path").map(|path| path_text(&path));
        if is_start {
            self.started.insert(id.clone(), now_unix_millis);
            return Some(self.feed.upsert(ActivityItem {
                id,
                source: action.source,
                client_label: client,
                purpose,
                description: describe(action.kind, action.source).to_owned(),
                kind: action.kind,
                operation: action.operation,
                path,
                bytes: None,
                state: ActivityState::Running,
                started_at: format_rfc3339_millis(now_unix_millis),
                duration_ms: None,
            }));
        }
        // A completion: the same item, now finished. A completion whose start
        // this fold never saw (a one-shot record) starts `duration` ago.
        let duration = entry.duration_ms;
        let started = self.started.remove(&id).unwrap_or_else(|| {
            now_unix_millis
                .saturating_sub(duration.map_or(0, |ms| i64::try_from(ms).unwrap_or(i64::MAX)))
        });
        let duration = duration.or_else(|| u64::try_from(now_unix_millis - started).ok());
        let state = state_of(entry);
        let bytes = match action.kind {
            // A transfer's completion carries the artifact's length, which the
            // item keeps whether the transfer completed or failed
            // (ST-M009-011); its state says whether all of it moved. A failure
            // before the length was known records none (0), so none is shown.
            ActivityKind::FileSent | ActivityKind::FileReceived => arg_u64(entry, "len")
                .filter(|len| *len > 0 || matches!(state, ActivityState::Done { .. })),
            _ => None,
        };
        let previous = self.feed.item(&id).cloned();
        Some(
            self.feed.upsert(ActivityItem {
                id,
                source: action.source,
                client_label: client
                    .or_else(|| previous.as_ref().and_then(|p| p.client_label.clone())),
                purpose: purpose.or_else(|| previous.as_ref().and_then(|p| p.purpose.clone())),
                description: describe(action.kind, action.source).to_owned(),
                kind: action.kind,
                operation: previous
                    .as_ref()
                    .map_or(action.operation, |p| p.operation.clone()),
                path: path.or_else(|| previous.as_ref().and_then(|p| p.path.clone())),
                bytes,
                state,
                started_at: format_rfc3339_millis(started),
                duration_ms: duration,
            }),
        )
    }

    /// Records that `count` live records were not shown at this place (the
    /// bounded live echo overflowed), returning the gap entry.
    pub fn shed(&mut self, count: u64, now_unix_millis: i64) -> ActivityEntry {
        self.feed.gap(count, format_rfc3339_millis(now_unix_millis))
    }

    /// Marks every item still running as failed because the session ended
    /// (never as done, REQ-N-008), returning the changed entries.
    pub fn session_ended(&mut self, now_unix_millis: i64) -> Vec<ActivityEntry> {
        let mut running: Vec<(String, i64)> = self.started.drain().collect();
        running.sort();
        running
            .into_iter()
            .filter_map(|(id, started)| {
                let mut item = self.feed.item(&id)?.clone();
                item.state = ActivityState::failed(SESSION_ENDED);
                item.duration_ms = u64::try_from(now_unix_millis.saturating_sub(started)).ok();
                Some(self.feed.upsert(item))
            })
            .collect()
    }

    /// The whole feed, oldest first.
    #[must_use]
    pub fn entries(&self) -> Vec<ActivityEntry> {
        self.feed.entries()
    }

    /// The item id of one record: the console-minted `call_id` of a routed
    /// call, the `action_id` of a file action of the technician, the `job_id`
    /// of a manual command, or a fresh local id for a one-shot completion
    /// record. A start record without a correlation id cannot be paired with
    /// its completion and produces no item.
    fn id_for(&mut self, entry: &RecordView<'_>, base: &str, is_start: bool) -> Option<String> {
        if let Some(call) = arg_u64(entry, "call_id") {
            return Some(format!("call-{call}"));
        }
        // A technician action is numbered apart from the routed calls, so the
        // two never share an item (ST-M010-001).
        if entry.actor == Actor::Console
            && let Some(action) = arg_u64(entry, "action_id")
        {
            return Some(format!("action-{action}"));
        }
        // A manual command's start and finish share its job id.
        if entry.actor == Actor::Console
            && matches!(base, "console_exec" | "console_exec_finish")
            && let Some(job) = arg_u64(entry, "job_id")
        {
            return Some(format!("job-{job}"));
        }
        if is_start {
            return None;
        }
        self.last_local += 1;
        Some(format!("local-{}", self.last_local))
    }
}

/// What one record's action is, or `None` for a record that is not an action
/// on the customer computer.
fn action_of(entry: &RecordView<'_>, base: &str) -> Option<Action> {
    match entry.actor {
        Actor::Mcp => {
            let tool = base.strip_prefix("mcp_remote_")?;
            let (kind, operation) = mcp_action(entry, tool)?;
            Some(Action {
                kind,
                source: ActivitySource::Ai,
                operation,
            })
        }
        Actor::Terminal => {
            let (kind, operation) = match base {
                "terminal_open" => (
                    ActivityKind::Command,
                    match arg_text(entry, "size") {
                        Some(size) => format!("open an interactive terminal ({size})"),
                        None => "open an interactive terminal".to_owned(),
                    },
                ),
                "terminal_close" => (
                    ActivityKind::JobControl,
                    format!("close terminal job {}", arg(entry, "job_id")),
                ),
                // Attaching or detaching a local terminal view is a registry
                // operation on the technician computer; nothing reaches the
                // customer computer.
                _ => return None,
            };
            Some(Action {
                kind,
                source: ActivitySource::Terminal,
                operation,
            })
        }
        Actor::Console => {
            let (kind, operation) = match base {
                "console_exec" | "console_exec_finish" => (
                    ActivityKind::Command,
                    format!("run a command: {}", arg(entry, "command")),
                ),
                "console_cancel" => (
                    ActivityKind::JobControl,
                    format!("stop job {}", arg(entry, "job_id")),
                ),
                // The technician's file explorer (ST-M010-001), in the words
                // the customer's own feed uses for the same actions.
                "console_fs_drives" => (ActivityKind::FileOperation, "list the drives".to_owned()),
                "console_remove_tree" => (
                    ActivityKind::FileOperation,
                    format!(
                        "delete the folder {} and everything in it",
                        path_text(&arg(entry, "path"))
                    ),
                ),
                "console_send" => (
                    ActivityKind::FileReceived,
                    format!(
                        "copy a file to the customer computer at {}",
                        path_text(&arg(entry, "path"))
                    ),
                ),
                // Lifecycle records (pairing, compare, consent, revoke) are
                // session events, not actions on the customer computer.
                _ => return None,
            };
            Some(Action {
                kind,
                source: ActivitySource::Technician,
                operation: bounded(operation),
            })
        }
        Actor::Agent | Actor::System => None,
    }
}

/// The kind and real operation of one MCP tool, from its audit arguments.
fn mcp_action(entry: &RecordView<'_>, tool: &str) -> Option<(ActivityKind, String)> {
    let path = || path_text(&arg(entry, "path"));
    let (kind, operation) = match tool {
        "system_info" => (
            ActivityKind::SystemInformation,
            "read system information".to_owned(),
        ),
        "exec" => (
            ActivityKind::Command,
            format!("run a command: {}", arg(entry, "command")),
        ),
        "write_stdin" => (
            ActivityKind::JobControl,
            format!(
                "send {} bytes of input to job {}",
                arg(entry, "input_len"),
                arg(entry, "job_id")
            ),
        ),
        "cancel" => (
            ActivityKind::JobControl,
            format!("stop job {}", arg(entry, "job_id")),
        ),
        "list_jobs" => (ActivityKind::JobControl, "list running jobs".to_owned()),
        "fs_stat" => (ActivityKind::FileOperation, format!("look up {}", path())),
        "fs_list" => (
            ActivityKind::FileOperation,
            format!("list the folder {}", path()),
        ),
        "make_dir" => (
            ActivityKind::FileOperation,
            format!("create the folder {}", path()),
        ),
        "move" => (
            ActivityKind::FileOperation,
            format!("move {} to {}", path(), path_text(&arg(entry, "to"))),
        ),
        "delete" => (
            ActivityKind::FileOperation,
            format!("delete the file {}", path()),
        ),
        "remove_dir" => (
            ActivityKind::FileOperation,
            format!("remove the folder {}", path()),
        ),
        "upload" => (
            ActivityKind::FileReceived,
            format!("copy a file to the customer computer at {}", path()),
        ),
        "download" => (
            ActivityKind::FileSent,
            format!("copy the file {} from the customer computer", path()),
        ),
        "list_staged" => (
            ActivityKind::FileOperation,
            "list the files CretLink staged".to_owned(),
        ),
        "cleanup" => (
            ActivityKind::FileOperation,
            format!("remove files CretLink staged ({})", arg(entry, "trigger")),
        ),
        "capture_desktop" => (
            ActivityKind::ScreenObserved,
            format!("take a screenshot of display {}", arg(entry, "display")),
        ),
        "desktop_pointer" => (
            ActivityKind::ScreenControlled,
            format!(
                "pointer {} with the {} button at ({}, {})",
                arg(entry, "action").to_lowercase(),
                arg(entry, "button").to_lowercase(),
                arg(entry, "x"),
                arg(entry, "y")
            ),
        ),
        "desktop_scroll" => (
            ActivityKind::ScreenControlled,
            format!(
                "scroll by ({}, {}) at ({}, {})",
                arg(entry, "dx"),
                arg(entry, "dy"),
                arg(entry, "x"),
                arg(entry, "y")
            ),
        ),
        "desktop_key" => (
            ActivityKind::ScreenControlled,
            format!(
                "key {} {}",
                arg(entry, "action").to_lowercase(),
                arg(entry, "key")
            ),
        ),
        "desktop_type" => (
            ActivityKind::ScreenControlled,
            format!("type {} bytes of text", arg(entry, "text_len")),
        ),
        "desktop_hotkey" => (
            ActivityKind::ScreenControlled,
            format!("press {}", arg(entry, "keys")),
        ),
        "desktop_wait" => (
            ActivityKind::ScreenControlled,
            format!("wait {} ms", arg(entry, "millis")),
        ),
        "clipboard_get" => (
            ActivityKind::ClipboardRead,
            "read the clipboard text".to_owned(),
        ),
        "clipboard_set" => (
            ActivityKind::ClipboardWritten,
            format!(
                "replace the clipboard text ({} bytes)",
                arg(entry, "text_len")
            ),
        ),
        _ => return None,
    };
    Some((kind, bounded(operation)))
}

/// The honest state of a completion record.
fn state_of(entry: &RecordView<'_>) -> ActivityState {
    let exit_code = match entry.args.0.get("exit_code") {
        Some(AuditValue::Int(code)) => i32::try_from(*code).ok(),
        _ => None,
    };
    match entry.outcome {
        Outcome::Ok => ActivityState::Done { exit_code },
        Outcome::Failed => ActivityState::Failed {
            exit_code,
            message: arg_text(entry, "reason"),
        },
        Outcome::Cancelled => ActivityState::Cancelled,
        Outcome::Timeout => ActivityState::TimedOut,
        Outcome::Unknown => ActivityState::Failed {
            exit_code,
            message: Some(UNKNOWN_RESULT.to_owned()),
        },
    }
}

/// One argument's display form (`[REDACTED]`/`[TRUNCATED]` markers kept), on
/// one line, or `?` when absent.
fn arg(entry: &RecordView<'_>, key: &str) -> String {
    arg_text(entry, key).unwrap_or_else(|| "?".to_owned())
}

fn arg_text(entry: &RecordView<'_>, key: &str) -> Option<String> {
    entry
        .args
        .0
        .get(key)
        .map(|value| value.to_string().replace(['\r', '\n'], " "))
}

fn arg_u64(entry: &RecordView<'_>, key: &str) -> Option<u64> {
    match entry.args.0.get(key) {
        Some(AuditValue::Int(n)) => u64::try_from(*n).ok(),
        _ => None,
    }
}

/// A remote path as the customer side words it: a staged artifact by name in
/// the CretLink staging folder.
fn path_text(path: &str) -> String {
    match path.strip_prefix("staging:") {
        Some(name) => format!("{name} (CretLink staging folder)"),
        None => path.to_owned(),
    }
}

/// Bounds an operation text like the customer side does, marking a cut.
fn bounded(text: String) -> String {
    if text.chars().count() <= MAX_ACTION_DESCRIPTION_CHARS {
        return text;
    }
    let mut out: String = text.chars().take(MAX_ACTION_DESCRIPTION_CHARS).collect();
    out.push_str(" [shortened]");
    out
}

#[cfg(test)]
mod tests;
