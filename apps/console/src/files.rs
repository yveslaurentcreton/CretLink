//! The technician's own file actions, audited (ST-M010-001, ST-M010-002,
//! REQ-F-017, REQ-S-017).
//!
//! [`ConsoleSessionService`] offers the operations of the technician's file
//! explorer: the drive list, the listing of a folder, the removal of a folder
//! with its content, a file sent into a folder, a file copied to the
//! technician computer, and the changes to one entry (a new folder, a move or
//! a rename, a delete, a look-up). [`TechnicianFiles`] is their audited front.
//! It runs each one on the service and records it in the console's audit trail
//! the way the IPC server records a routed AI call: a capture-time start
//! record before anything reaches the target, then a completion record with
//! the honest outcome. The records are attributed to the technician (actor
//! `console`), and the service stamps every request [`Origin::Console`], so
//! the customer sees them as the technician's as well.
//!
//! A screen calls this front, never the service's operations directly, so no
//! action of the technician can miss the audit trail.
//!
//! # The records
//!
//! | Start, completion | Arguments |
//! |---|---|
//! | `console_fs_drives_start`, `console_fs_drives` | `drives`: how many were listed |
//! | `console_fs_list_start`, `console_fs_list` | `path`: the place as it was asked for, then the folder that was listed, as the target names it; `entries`: how many entries were listed; `more` when the listing stopped at its bound and the folder holds more |
//! | `console_remove_tree_start`, `console_remove_tree` | `path`; `removed_files`, `removed_dirs`; `not_removed` when entries were left behind |
//! | `console_send_start`, `console_send` | `path`: where the file goes, then the path the target committed it at; `overwrite` when a replacement was asked for; `len`; the digest `sha256` |
//! | `console_download_start`, `console_download` | `path`: the file on the target; `len`; the digest `sha256` of a copy that was verified |
//! | `console_fs_stat_start`, `console_fs_stat` | `path`; `found`: whether an entry lies there; `part` when an unfinished part of a transfer does |
//! | `console_make_dir_start`, `console_make_dir` | `path` |
//! | `console_move_start`, `console_move` | `path`: the entry; `to`: its new place |
//! | `console_delete_start`, `console_delete` | `path` |
//!
//! Both records of one action carry the same `action_id`, minted by the
//! service and unique in the session. A completion carries the measured
//! duration and, unless it succeeded, the `reason`. Its outcome is `ok` only
//! for a folder that is gone or a file that is committed and verified;
//! a cancelled action is `cancelled`, and one whose session was lost before
//! the answer is `unknown`, never `ok` (REQ-N-008). A send refused before it
//! began (a name that is not one name, a folder that is not a rooted path) is
//! one failed `console_send_start` record: nothing reached the target. The
//! same holds for a folder whose path is no remote path: one failed
//! `console_fs_list_start` record.
//!
//! One listing is one pair of records, however many pages the folder takes
//! and however many requests it took to find it (ST-M010-002). A listing the
//! technician left for another place is `cancelled`, with what had been
//! listed until then.
//!
//! The local path of a file being sent, or of the copy a download makes, is
//! not recorded, as for a transfer of the AI; no file content ever is
//! (REQ-S-018).
//!
//! [`Origin::Console`]: cretlink_session::Origin::Console

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use cretlink_mcp::{DownloadReport, StatReport, TargetError, UploadReport};
use cretlink_session::{
    CancelToken, DriveEntry, JobOutcome, RemotePath, TransferProgress, TreeRemovalProgress,
    TreeRemovalReport,
};
use cretlink_shared::{Actor, AuditArgs, AuditValue, Outcome, truncate_field};

use crate::explorer::{self, FolderEvent, FolderReport, ListingEnd};
use crate::ipc_server::{
    AuditEntry, IpcAudit, elapsed_ms, job_outcome_to_audit, len_value, remote_path_value,
    transfer_detail,
};
use crate::service::{ConsoleSessionService, ProgressSink, SendToFolder};

/// The audited front of the technician's own file actions on one consented
/// session (ST-M010-001): each call runs on the session service and is
/// recorded in the console's audit trail before and after it.
///
/// It borrows the service and the audit sink, so it is made where it is used
/// and keeps neither alive.
#[derive(Clone, Copy)]
pub struct TechnicianFiles<'a> {
    service: &'a ConsoleSessionService,
    audit: &'a dyn IpcAudit,
}

impl std::fmt::Debug for TechnicianFiles<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TechnicianFiles")
            .field("service", self.service)
            .finish_non_exhaustive()
    }
}

impl<'a> TechnicianFiles<'a> {
    /// The front for the session `service` owns, recording into `audit`.
    #[must_use]
    pub fn new(service: &'a ConsoleSessionService, audit: &'a dyn IpcAudit) -> Self {
        Self { service, audit }
    }

    /// Lists the drives of the target, like
    /// [`ConsoleSessionService::list_drives`], and records it.
    ///
    /// # Errors
    ///
    /// See [`ConsoleSessionService::list_drives`]. The error is recorded.
    pub async fn list_drives(&self) -> Result<Vec<DriveEntry>, TargetError> {
        let action = self.begin(
            "console_fs_drives_start",
            "console_fs_drives",
            AuditArgs::new(),
        );
        let result = self.service.list_drives().await;
        let mut entry = action.complete(&result);
        if let Ok(drives) = &result {
            entry.args.insert("drives", len_value(drives.len() as u64));
        }
        self.audit.record(entry);
        result
    }

    /// Opens the place that the path `asked` names and lists it, for the
    /// technician's file explorer, and records it (ST-M010-002).
    ///
    /// The place is found in the target's own names, whatever the spelling of
    /// `asked`: the root of its drive as the drive list gives it, and each
    /// further name as a listing of the folder above it gives it. A path that
    /// names a file leads to the folder that holds it. The folder is then
    /// listed page by page until its end. `sink` gets the pages as they
    /// arrive, the first one with the folder's real location
    /// ([`FolderEvent`]); nothing is delivered unless the folder was found
    /// and the target answered its first page. See [`crate::explorer`].
    ///
    /// Setting `cancel` ends the listing between two requests; a revoke or a
    /// stop ends it with the session.
    ///
    /// # Errors
    ///
    /// A [`TargetError`]: a path that is no remote path (nothing is asked of
    /// the target), a name the folder above it does not list, a link on the
    /// way, a refusal of the target, or the end of the session. The error is
    /// recorded, with what had been listed until then.
    pub async fn open_folder(
        &self,
        asked: &str,
        cancel: CancelToken,
        sink: &mut (dyn FnMut(FolderEvent) + Send),
    ) -> Result<FolderReport, TargetError> {
        if let Err(error) = explorer::spell(asked) {
            // Refused before it began: one failed start record, with the
            // path as it was asked for. Nothing reached the target.
            let mut entry = AuditEntry::event(
                Actor::Console,
                Some(self.service.session()),
                "console_fs_list_start",
                Outcome::Failed,
            );
            entry.args = AuditArgs::new()
                .with("path", truncate_field(asked))
                .with("reason", truncate_field(&error.to_string()));
            self.audit.record(entry);
            return Err(error);
        }
        let action = self.begin(
            "console_fs_list_start",
            "console_fs_list",
            AuditArgs::new().with("path", truncate_field(asked)),
        );
        let mut seen = Listed::default();
        let result = {
            let mut tap = |event: FolderEvent| {
                seen.note(&event);
                sink(event);
            };
            explorer::open(
                self.service,
                self.service.browsed(),
                asked,
                &cancel,
                &mut tap,
            )
            .await
        };
        self.audit.record(listing_record(action, &result, &seen));
        result
    }

    /// Lists the folder `path` of the target page by page until its end, and
    /// records it (ST-M010-002). Unlike [`Self::open_folder`] it takes the
    /// folder as it is named, a staged folder included, and looks nothing up.
    ///
    /// # Errors
    ///
    /// A [`TargetError`]: a refusal of the target, or the end of the session.
    /// The error is recorded, with what had been listed until then.
    pub async fn list_folder(
        &self,
        path: RemotePath,
        cancel: CancelToken,
        sink: &mut (dyn FnMut(FolderEvent) + Send),
    ) -> Result<FolderReport, TargetError> {
        let action = self.begin(
            "console_fs_list_start",
            "console_fs_list",
            AuditArgs::new().with("path", remote_path_value(&path)),
        );
        let mut seen = Listed::default();
        let result = {
            let mut tap = |event: FolderEvent| {
                seen.note(&event);
                sink(event);
            };
            explorer::list(
                self.service,
                self.service.browsed(),
                &path,
                None,
                &cancel,
                &mut tap,
            )
            .await
            .map(|(entries, end)| FolderReport {
                folder: Some(path),
                select: None,
                entries,
                end,
            })
        };
        self.audit.record(listing_record(action, &result, &seen));
        result
    }

    /// Removes a folder of the target with everything in it, like
    /// [`ConsoleSessionService::remove_tree`], and records it with what was
    /// removed and what was left behind.
    ///
    /// # Errors
    ///
    /// See [`ConsoleSessionService::remove_tree`]. The error is recorded, with
    /// what had been removed until then.
    pub async fn remove_tree(
        &self,
        path: RemotePath,
        cancel: CancelToken,
        mut progress: Option<ProgressSink<TreeRemovalProgress>>,
    ) -> Result<TreeRemovalReport, TargetError> {
        let action = self.begin(
            "console_remove_tree_start",
            "console_remove_tree",
            AuditArgs::new().with("path", remote_path_value(&path)),
        );
        // The counts of the last step that was answered, so a removal the
        // session did not carry to its end is still recorded with what it
        // had removed.
        let seen = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
        let tap = Arc::clone(&seen);
        let sink: ProgressSink<TreeRemovalProgress> = Box::new(move |step| {
            tap.0.store(step.removed_files, Ordering::SeqCst);
            tap.1.store(step.removed_dirs, Ordering::SeqCst);
            if let Some(sink) = progress.as_mut() {
                sink(step);
            }
        });
        let result = self.service.remove_tree(path, cancel, Some(sink)).await;
        let mut entry = action.complete(&result);
        match &result {
            Ok(report) => removal_detail(&mut entry, report),
            Err(_) => {
                let files = seen.0.load(Ordering::SeqCst);
                let dirs = seen.1.load(Ordering::SeqCst);
                if files > 0 || dirs > 0 {
                    entry.args.insert("removed_files", len_value(files));
                    entry.args.insert("removed_dirs", len_value(dirs));
                }
            }
        }
        self.audit.record(entry);
        result
    }

    /// Sends a file into a folder of the target, like
    /// [`ConsoleSessionService::send_to_folder`], and records it with the
    /// path the target committed it at, its length and its verified digest.
    ///
    /// # Errors
    ///
    /// See [`ConsoleSessionService::send_to_folder`]. The error is recorded.
    pub async fn send_to_folder(
        &self,
        send: SendToFolder,
        cancel: CancelToken,
        progress: Option<ProgressSink<TransferProgress>>,
    ) -> Result<UploadReport, TargetError> {
        let dest = match send.dest() {
            Ok(dest) => dest,
            Err(error) => {
                // Refused before it began: one failed start record, as for a
                // command the console refused to start. The path is recorded
                // as it was asked for.
                let mut entry = AuditEntry::event(
                    Actor::Console,
                    Some(self.service.session()),
                    "console_send_start",
                    Outcome::Failed,
                );
                entry.args = AuditArgs::new()
                    .with(
                        "path",
                        truncate_field(&format!("{}\\{}", send.folder, send.name)),
                    )
                    .with("reason", truncate_field(&error.to_string()));
                self.audit.record(entry);
                return Err(error);
            }
        };
        let mut args = AuditArgs::new().with("path", remote_path_value(&dest));
        if send.overwrite {
            args.insert("overwrite", AuditValue::Bool(true));
        }
        let action = self.begin("console_send_start", "console_send", args);
        let result = self.service.send_to_folder(send, cancel, progress).await;
        let mut entry = action.complete(&result);
        if let Ok(report) = &result {
            transfer_detail(
                &mut entry,
                report.remote_path.as_deref(),
                report.len,
                report.resumed_from,
                report.restarted,
                &report.digest,
                report.error.as_ref().map(|error| error.message.as_str()),
                report.outcome,
            );
        }
        self.audit.record(entry);
        result
    }

    /// Copies a file of the target to `dest` on the technician computer,
    /// like [`ConsoleSessionService::download_file`], and records it with the
    /// file's path on the target, its length and the digest that was verified
    /// (ST-M010-002). The local path is not recorded.
    ///
    /// # Errors
    ///
    /// See [`ConsoleSessionService::download_file`]. The error is recorded.
    pub async fn download(
        &self,
        source: RemotePath,
        dest: std::path::PathBuf,
        cancel: CancelToken,
        progress: Option<ProgressSink<TransferProgress>>,
    ) -> Result<DownloadReport, TargetError> {
        let action = self.begin(
            "console_download_start",
            "console_download",
            AuditArgs::new().with("path", remote_path_value(&source)),
        );
        let result = self
            .service
            .download_file(source, dest, cancel, progress)
            .await;
        let mut entry = action.complete(&result);
        if let Ok(report) = &result {
            transfer_detail(
                &mut entry,
                None,
                report.len,
                report.resumed_from,
                report.restarted,
                &report.digest,
                report.error.as_ref().map(|error| error.message.as_str()),
                report.outcome,
            );
        }
        self.audit.record(entry);
        result
    }

    /// Looks one path of the target up, like
    /// [`ConsoleSessionService::stat_entry`], and records it with whether an
    /// entry lies there and whether an unfinished part of a transfer does
    /// (ST-M010-002).
    ///
    /// # Errors
    ///
    /// See [`ConsoleSessionService::stat_entry`]. The error is recorded.
    pub async fn look_up(&self, path: RemotePath) -> Result<StatReport, TargetError> {
        let action = self.begin(
            "console_fs_stat_start",
            "console_fs_stat",
            AuditArgs::new().with("path", remote_path_value(&path)),
        );
        let result = self.service.stat_entry(path).await;
        let mut entry = action.complete(&result);
        if let Ok(report) = &result {
            entry
                .args
                .insert("found", AuditValue::Bool(report.entry.is_some()));
            if report.partial_len.is_some() {
                entry.args.insert("part", AuditValue::Bool(true));
            }
        }
        self.audit.record(entry);
        result
    }

    /// Creates one folder on the target, like
    /// [`ConsoleSessionService::create_folder`], and records it
    /// (ST-M010-002).
    ///
    /// # Errors
    ///
    /// See [`ConsoleSessionService::create_folder`]. The error is recorded.
    pub async fn create_folder(&self, path: RemotePath) -> Result<(), TargetError> {
        let action = self.begin(
            "console_make_dir_start",
            "console_make_dir",
            AuditArgs::new().with("path", remote_path_value(&path)),
        );
        let result = self.service.create_folder(path).await;
        self.audit.record(action.complete(&result));
        result
    }

    /// Moves or renames one entry of the target, like
    /// [`ConsoleSessionService::move_entry`], and records it with both places
    /// (ST-M010-002). A destination that is taken is refused.
    ///
    /// # Errors
    ///
    /// See [`ConsoleSessionService::move_entry`]. The error is recorded.
    pub async fn rename(&self, from: RemotePath, to: RemotePath) -> Result<(), TargetError> {
        let action = self.begin(
            "console_move_start",
            "console_move",
            AuditArgs::new()
                .with("path", remote_path_value(&from))
                .with("to", remote_path_value(&to)),
        );
        let result = self.service.move_entry(from, to).await;
        self.audit.record(action.complete(&result));
        result
    }

    /// Deletes one file or one link of the target, like
    /// [`ConsoleSessionService::delete_entry`], and records it (ST-M010-002).
    ///
    /// # Errors
    ///
    /// See [`ConsoleSessionService::delete_entry`]. The error is recorded.
    pub async fn delete(&self, path: RemotePath) -> Result<(), TargetError> {
        let action = self.begin(
            "console_delete_start",
            "console_delete",
            AuditArgs::new().with("path", remote_path_value(&path)),
        );
        let result = self.service.delete_entry(path).await;
        self.audit.record(action.complete(&result));
        result
    }

    /// Records the capture-time start of one action, before anything reaches
    /// the target, and returns the envelope that builds its completion.
    fn begin(
        &self,
        action_start: &'static str,
        action: &'static str,
        mut args: AuditArgs,
    ) -> Recorded {
        let session = self.service.session();
        args.insert(
            "action_id",
            len_value(self.service.next_technician_action()),
        );
        let mut start = AuditEntry::event(Actor::Console, Some(session), action_start, Outcome::Ok);
        start.args = args.clone();
        self.audit.record(start);
        Recorded {
            session,
            action,
            args,
            started: Instant::now(),
        }
    }
}

/// The two-record audit envelope of one technician action: what
/// [`TechnicianFiles::begin`] recorded as the start, kept to build the
/// completion with the same `action_id` and arguments.
struct Recorded {
    session: cretlink_protocol::SessionId,
    action: &'static str,
    args: AuditArgs,
    started: Instant,
}

impl Recorded {
    /// The completion record of a call that returned `result`: the measured
    /// duration, the reason of an error, and an outcome that is `ok` only for
    /// an answer. A lost session leaves the true outcome unknown, never a
    /// fabricated success or failure (REQ-N-008). An answer that is itself a
    /// report gets its own outcome from the caller.
    fn complete<T>(self, result: &Result<T, TargetError>) -> AuditEntry {
        let outcome = match result {
            Ok(_) => Outcome::Ok,
            Err(TargetError::Disconnected) => Outcome::Unknown,
            Err(
                TargetError::NoConsentedSession
                | TargetError::Remote { .. }
                | TargetError::Unexpected,
            ) => Outcome::Failed,
        };
        let mut entry = AuditEntry::event(Actor::Console, Some(self.session), self.action, outcome);
        entry.args = self.args;
        entry.duration_ms = Some(elapsed_ms(self.started));
        if let Err(error) = result {
            entry
                .args
                .insert("reason", truncate_field(&error.to_string()));
        }
        entry
    }
}

/// What a listing delivered so far: the folder it opened and how many
/// entries. Kept beside the listing so that one the session did not carry to
/// its end is still recorded with what it had listed.
#[derive(Debug, Default)]
struct Listed {
    folder: Option<RemotePath>,
    entries: u64,
}

impl Listed {
    fn note(&mut self, event: &FolderEvent) {
        let delivered = match event {
            FolderEvent::Opened {
                folder, entries, ..
            } => {
                self.folder = Some(folder.clone());
                entries.len()
            }
            FolderEvent::Page { entries, .. } => entries.len(),
        };
        self.entries = self.entries.saturating_add(delivered as u64);
    }
}

/// The completion record of a listing that returned `result` after delivering
/// `seen`: the folder that was listed, as the target names it, and the number
/// of entries. Its outcome is `ok` for a listing that reached the end of the
/// folder or its bound (then with `more`), `cancelled` for one the technician
/// left, and never `ok` for one that failed or lost its session (REQ-N-008).
fn listing_record(
    action: Recorded,
    result: &Result<FolderReport, TargetError>,
    seen: &Listed,
) -> AuditEntry {
    let mut entry = action.complete(result);
    let (folder, entries) = match result {
        Ok(report) => (report.folder.as_ref(), report.entries),
        Err(_) => (seen.folder.as_ref(), seen.entries),
    };
    if let Some(folder) = folder {
        // The folder as the target names it supersedes the path as asked.
        entry.args.insert("path", remote_path_value(folder));
        entry.args.insert("entries", len_value(entries));
    }
    if let Ok(report) = result {
        match report.end {
            ListingEnd::Complete => {}
            ListingEnd::Cancelled => entry.outcome = Outcome::Cancelled,
            ListingEnd::Limit => {
                entry.args.insert("more", AuditValue::Bool(true));
            }
        }
    }
    entry
}

/// Fills the completion record of a folder removal from its honest report:
/// what was removed, how many entries were left behind and the first of them
/// with its reason, and the mapped outcome — `ok` only for a folder that is
/// gone (REQ-N-008).
fn removal_detail(entry: &mut AuditEntry, report: &TreeRemovalReport) {
    entry
        .args
        .insert("removed_files", len_value(report.removed_files));
    entry
        .args
        .insert("removed_dirs", len_value(report.removed_dirs));
    let not_removed = (report.failed.len() as u64).saturating_add(report.failed_more);
    if not_removed > 0 {
        entry.args.insert("not_removed", len_value(not_removed));
    }
    if report.outcome == JobOutcome::Failed {
        entry
            .args
            .insert("reason", truncate_field(&removal_failure(report)));
    }
    entry.outcome = job_outcome_to_audit(report.outcome);
}

/// One line on why a folder removal failed: the step the target refused, or
/// how many entries could not be removed and the first of them.
fn removal_failure(report: &TreeRemovalReport) -> String {
    if let Some(error) = &report.error {
        return format!("{:?}: {}", error.code, error.message);
    }
    let not_removed = (report.failed.len() as u64).saturating_add(report.failed_more);
    let Some(first) = report.failed.first() else {
        return "the folder could not be removed".to_owned();
    };
    let entries = if not_removed == 1 { "entry" } else { "entries" };
    let mut reason = format!(
        "{not_removed} {entries} could not be removed, the first: {} ({:?})",
        first.path, first.code
    );
    if report.blocked {
        reason.push_str("; the removal gave up before it had tried every entry");
    }
    reason
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use cretlink_protocol::{ErrorCode, TreeFailure};
    use cretlink_session::TreeRemovalError;

    use super::*;

    fn report(outcome: JobOutcome) -> TreeRemovalReport {
        TreeRemovalReport {
            outcome,
            removed_files: 7,
            removed_dirs: 2,
            failed: Vec::new(),
            failed_more: 0,
            blocked: false,
            error: None,
        }
    }

    fn completion(report: &TreeRemovalReport) -> AuditEntry {
        let mut entry = AuditEntry::event(Actor::Console, None, "console_remove_tree", Outcome::Ok);
        removal_detail(&mut entry, report);
        entry
    }

    fn int(entry: &AuditEntry, key: &str) -> Option<i64> {
        match entry.args.0.get(key) {
            Some(AuditValue::Int(value)) => Some(*value),
            _ => None,
        }
    }

    fn text(entry: &AuditEntry, key: &str) -> String {
        entry
            .args
            .0
            .get(key)
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    /// A folder that is gone is the only removal recorded as `ok`.
    #[test]
    fn only_a_folder_that_is_gone_is_recorded_as_ok() {
        let done = completion(&report(JobOutcome::Completed));
        assert_eq!(done.outcome, Outcome::Ok);
        assert_eq!(int(&done, "removed_files"), Some(7));
        assert_eq!(int(&done, "removed_dirs"), Some(2));
        assert_eq!(int(&done, "not_removed"), None);
        assert!(!done.args.0.contains_key("reason"));

        let cancelled = completion(&report(JobOutcome::Cancelled));
        assert_eq!(cancelled.outcome, Outcome::Cancelled);
        assert_eq!(int(&cancelled, "removed_files"), Some(7));
        assert!(!cancelled.args.0.contains_key("reason"));

        let unknown = completion(&report(JobOutcome::Unknown));
        assert_eq!(unknown.outcome, Outcome::Unknown);
    }

    /// What was left behind is counted, and the first entry is named with its
    /// reason.
    #[test]
    fn a_removal_that_left_entries_behind_says_how_many_and_which_first() {
        let mut left = report(JobOutcome::Failed);
        left.failed = vec![
            TreeFailure {
                path: "C:\\Users\\ann\\Documents\\old\\open.docx".to_owned(),
                code: ErrorCode::FileInUse,
            },
            TreeFailure {
                path: "C:\\Users\\ann\\Documents\\old".to_owned(),
                code: ErrorCode::DirNotEmpty,
            },
        ];
        left.failed_more = 3;
        let entry = completion(&left);
        assert_eq!(entry.outcome, Outcome::Failed);
        assert_eq!(int(&entry, "not_removed"), Some(5));
        assert_eq!(
            text(&entry, "reason"),
            "5 entries could not be removed, the first: \
             C:\\Users\\ann\\Documents\\old\\open.docx (FileInUse)"
        );

        left.failed.truncate(1);
        left.failed_more = 0;
        left.blocked = true;
        assert_eq!(
            text(&completion(&left), "reason"),
            "1 entry could not be removed, the first: \
             C:\\Users\\ann\\Documents\\old\\open.docx (FileInUse); \
             the removal gave up before it had tried every entry"
        );
    }

    /// A step the target refused is recorded with the target's own reason.
    #[test]
    fn a_refused_removal_is_recorded_with_the_reason_of_the_target() {
        let mut refused = report(JobOutcome::Failed);
        refused.removed_files = 0;
        refused.removed_dirs = 0;
        refused.error = Some(TreeRemovalError {
            code: ErrorCode::PathNotFound,
            message: "the folder does not exist".to_owned(),
        });
        let entry = completion(&refused);
        assert_eq!(entry.outcome, Outcome::Failed);
        assert_eq!(int(&entry, "not_removed"), None);
        assert_eq!(
            text(&entry, "reason"),
            "PathNotFound: the folder does not exist"
        );
    }

    // --- A folder listed for the explorer (ST-M010-002) ---

    fn listing_action() -> Recorded {
        Recorded {
            session: cretlink_protocol::SessionId::new(),
            action: "console_fs_list",
            args: AuditArgs::new()
                .with("action_id", len_value(4))
                .with("path", truncate_field("c:/users/ANN/documents")),
            started: Instant::now(),
        }
    }

    fn documents() -> RemotePath {
        RemotePath::Absolute {
            path: "C:\\Users\\ann\\Documents".to_owned(),
        }
    }

    fn listed(end: ListingEnd, entries: u64) -> Result<FolderReport, TargetError> {
        Ok(FolderReport {
            folder: Some(documents()),
            select: None,
            entries,
            end,
        })
    }

    /// A listing that reached the end of its folder is recorded as `ok`,
    /// with the folder as the target names it and the number of entries.
    #[test]
    fn a_listing_is_recorded_with_the_real_folder_and_its_entries() {
        let seen = Listed::default();
        let done = listing_record(listing_action(), &listed(ListingEnd::Complete, 4321), &seen);
        assert_eq!(done.outcome, Outcome::Ok);
        assert_eq!(done.action, "console_fs_list");
        assert_eq!(text(&done, "path"), "C:\\Users\\ann\\Documents");
        assert_eq!(int(&done, "entries"), Some(4321));
        assert_eq!(int(&done, "action_id"), Some(4));
        assert!(!done.args.0.contains_key("more"));
        assert!(!done.args.0.contains_key("reason"));
        assert!(done.duration_ms.is_some());

        // An empty folder was listed too: none of its entries, to its end.
        let empty = listing_record(listing_action(), &listed(ListingEnd::Complete, 0), &seen);
        assert_eq!(empty.outcome, Outcome::Ok);
        assert_eq!(int(&empty, "entries"), Some(0));
    }

    /// A listing that stopped at its bound says that the folder holds more;
    /// one the technician left is `cancelled`, also before the folder was
    /// found, when the record keeps the path as it was asked for.
    #[test]
    fn a_listing_that_is_not_the_whole_folder_says_so() {
        let seen = Listed::default();
        let bound = listing_record(listing_action(), &listed(ListingEnd::Limit, 100_100), &seen);
        assert_eq!(bound.outcome, Outcome::Ok);
        assert_eq!(bound.args.0.get("more"), Some(&AuditValue::Bool(true)));
        assert_eq!(int(&bound, "entries"), Some(100_100));

        let left = listing_record(listing_action(), &listed(ListingEnd::Cancelled, 512), &seen);
        assert_eq!(left.outcome, Outcome::Cancelled);
        assert_eq!(int(&left, "entries"), Some(512));
        assert_eq!(text(&left, "path"), "C:\\Users\\ann\\Documents");

        let never_found = Ok(FolderReport {
            folder: None,
            select: None,
            entries: 0,
            end: ListingEnd::Cancelled,
        });
        let left = listing_record(listing_action(), &never_found, &seen);
        assert_eq!(left.outcome, Outcome::Cancelled);
        assert_eq!(text(&left, "path"), "c:/users/ANN/documents");
        assert_eq!(int(&left, "entries"), None);
    }

    /// A listing that failed or lost its session is never `ok`. It is
    /// recorded with what it had delivered until then, and a refusal before
    /// anything was listed with the path as it was asked for.
    #[test]
    fn a_listing_that_failed_is_recorded_with_what_it_had_listed() {
        let mut seen = Listed::default();
        seen.note(&FolderEvent::Opened {
            folder: documents(),
            select: None,
            entries: vec![entry("a.txt"), entry("b.txt")],
            end: false,
        });
        seen.note(&FolderEvent::Page {
            entries: vec![entry("c.txt")],
            end: false,
        });
        let lost = listing_record(listing_action(), &Err(TargetError::Disconnected), &seen);
        assert_eq!(lost.outcome, Outcome::Unknown);
        assert_eq!(text(&lost, "path"), "C:\\Users\\ann\\Documents");
        assert_eq!(int(&lost, "entries"), Some(3));
        assert!(text(&lost, "reason").contains("outcome unknown"));

        let stopped = listing_record(
            listing_action(),
            &Err(TargetError::NoConsentedSession),
            &seen,
        );
        assert_eq!(stopped.outcome, Outcome::Failed);
        assert_eq!(int(&stopped, "entries"), Some(3));

        let refused = listing_record(
            listing_action(),
            &Err(TargetError::Remote {
                code: cretlink_protocol::ErrorCode::AccessDenied,
                message: "failed to open the directory".to_owned(),
                stage: cretlink_protocol::ErrorStage::Execution,
            }),
            &Listed::default(),
        );
        assert_eq!(refused.outcome, Outcome::Failed);
        assert_eq!(text(&refused, "path"), "c:/users/ANN/documents");
        assert_eq!(int(&refused, "entries"), None);
        assert!(text(&refused, "reason").contains("AccessDenied"));
    }

    fn entry(name: &str) -> cretlink_protocol::FileEntry {
        cretlink_protocol::FileEntry {
            name: name.to_owned(),
            kind: cretlink_protocol::FileKind::File,
            len: 1,
            modified_ms: None,
            readonly: false,
        }
    }

    /// The outcome of a call that returned no answer: a lost session is
    /// unknown, never a success or a failure it did not see.
    #[test]
    fn a_lost_session_is_recorded_as_unknown() {
        let recorded = |result: &Result<(), TargetError>| {
            Recorded {
                session: cretlink_protocol::SessionId::new(),
                action: "console_fs_drives",
                args: AuditArgs::new().with("action_id", len_value(3)),
                started: Instant::now(),
            }
            .complete(result)
        };
        let lost = recorded(&Err(TargetError::Disconnected));
        assert_eq!(lost.outcome, Outcome::Unknown);
        assert_eq!(int(&lost, "action_id"), Some(3));
        assert!(text(&lost, "reason").contains("outcome unknown"));
        assert!(lost.duration_ms.is_some());

        assert_eq!(
            recorded(&Err(TargetError::NoConsentedSession)).outcome,
            Outcome::Failed
        );
        assert_eq!(
            recorded(&Err(TargetError::Unexpected)).outcome,
            Outcome::Failed
        );
        let done = recorded(&Ok(()));
        assert_eq!(done.outcome, Outcome::Ok);
        assert!(!done.args.0.contains_key("reason"));
    }
}
