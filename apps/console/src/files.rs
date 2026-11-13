//! The technician's own file actions, audited (ST-M010-001, REQ-F-017,
//! REQ-S-017).
//!
//! [`ConsoleSessionService`] offers the three operations the technician's file
//! explorer adds: the drive list, the removal of a folder with its content, and
//! a file sent into a folder. [`TechnicianFiles`] is their audited front. It
//! runs each one on the service and records it in the console's audit trail
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
//! | `console_remove_tree_start`, `console_remove_tree` | `path`; `removed_files`, `removed_dirs`; `not_removed` when entries were left behind |
//! | `console_send_start`, `console_send` | `path`: where the file goes, then the path the target committed it at; `overwrite` when a replacement was asked for; `len`; the digest `sha256` |
//!
//! Both records of one action carry the same `action_id`, minted by the
//! service and unique in the session. A completion carries the measured
//! duration and, unless it succeeded, the `reason`. Its outcome is `ok` only
//! for a folder that is gone or a file that is committed and verified;
//! a cancelled action is `cancelled`, and one whose session was lost before
//! the answer is `unknown`, never `ok` (REQ-N-008). A send refused before it
//! began (a name that is not one name, a folder that is not a rooted path) is
//! one failed `console_send_start` record: nothing reached the target.
//!
//! The local path of a file being sent is not recorded, as for an AI upload;
//! no file content ever is (REQ-S-018).
//!
//! [`Origin::Console`]: cretlink_session::Origin::Console

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use cretlink_mcp::{TargetError, UploadReport};
use cretlink_session::{
    CancelToken, DriveEntry, JobOutcome, RemotePath, TransferProgress, TreeRemovalProgress,
    TreeRemovalReport,
};
use cretlink_shared::{Actor, AuditArgs, AuditValue, Outcome, truncate_field};

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
